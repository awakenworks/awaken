//! Application bearer expiry uses the existing Managed generation transaction.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::repo::{InMemoryCredentialRepo, ManagedCredentialRepository};

fn entry(generation: u64, expiry: Option<u64>) -> crate::ApplicationMcpBearerEntry {
    crate::ApplicationMcpBearerEntry {
        workspace_id: "workspace".into(),
        application_authority_id: "expiry-application".into(),
        mcp_server_url: "https://flow.example.test/mcp".into(),
        target: crate::McpCredentialTarget {
            audience: "https://flow.example.test/mcp".into(),
            fingerprint: "expiry-endpoint".into(),
        },
        idempotency_key: format!("generation-{generation}"),
        credential_generation: generation,
        expires_at_unix_ms: expiry,
        bearer: RedactedString::new(format!("test-bearer-{generation}")), // awaken-allow(secret): deterministic test material
    }
}

fn expiry(source: &CredentialSource) -> Option<u64> {
    source.descriptor.as_ref().unwrap().expires_at_unix_ms
}

fn compiled(source: &CredentialSource) -> awaken_agent_contract::credential::CredentialAccess {
    use awaken_agent_contract::credential::*;
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker.test");
    let usage = CredentialUsage::HttpHeader {
        name: "authorization".into(),
        scheme: Some("Bearer".into()),
    };
    let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
        "workspace",
        &"mcp",
        &usage,
    );
    crate::compile_exact_credential_access(
        source,
        crate::ExactCredentialAccessRequest {
            workspace_id: Some("workspace"),
            target: Some(CredentialTarget::new(
                CredentialPurpose::McpAuthorization,
                "https://flow.example.test/mcp",
            )),
            usage,
            policy: CredentialExecutionPolicy::exact(
                holder.clone(),
                ModelExposurePolicy::Forbidden,
            ),
            selected_holder: &holder,
            binding: &binding,
            now_unix_ms: 1_000,
        },
    )
    .unwrap()
}

#[test]
fn application_target_constructor_preserves_the_published_normalized_identity() {
    let canonical =
        crate::McpCredentialTarget::parse_http("https://flow.example.test/mcp").unwrap();
    let cosmetic =
        crate::McpCredentialTarget::parse_http("HTTPS://FLOW.EXAMPLE.TEST:443/mcp/").unwrap();
    assert_eq!(canonical.audience, "https://flow.example.test/mcp");
    assert_eq!(canonical.audience, cosmetic.audience);
    assert_eq!(canonical.fingerprint, cosmetic.fingerprint);
    assert_eq!(
        canonical.source_id("workspace", "application"),
        cosmetic.source_id("workspace", "application")
    );
    assert_ne!(
        canonical.source_id("workspace", "application"),
        canonical.source_id("other-workspace", "application")
    );
    assert_ne!(
        canonical.source_id("workspace", "application"),
        canonical.source_id("workspace", "other-application")
    );
    #[cfg(target_pointer_width = "64")]
    assert_eq!(
        canonical.fingerprint, "b72a2c9b4c20bf2dd4fd60d6efff9f6e84e472a13a2764b7219fef0e6dae84e6",
        "persisted target identity retains its original length-framed SHA-256"
    );
    let query = crate::McpCredentialTarget::parse_http("https://flow.example.test/mcp?").unwrap();
    assert_ne!(
        query.fingerprint, canonical.fingerprint,
        "empty query stays distinct from no query"
    );
    for invalid in [
        "stdio://server",
        "https://user:password@example.test",
        "https://example.test/#fragment",
    ] {
        assert!(crate::McpCredentialTarget::parse_http(invalid).is_err());
    }
}

#[tokio::test]
async fn plain_application_command_uses_the_same_exact_expiry_generation_rule() {
    let repo = InMemoryCredentialRepo::new();
    let store = crate::InMemorySecretStore::new();
    let clock = awaken_lease_testkit::ManualClock::new(1_000);
    let command = |generation, expiry| ApplicationMcpBearerCommand {
        source_id: CredentialSourceId("cred:plain-expiry".into()),
        workspace_id: "workspace".into(),
        target_fingerprint: "endpoint".into(),
        target_audience: "https://flow.example.test/mcp".into(),
        command_key_fingerprint: format!("key-{generation}"),
        credential_generation: generation,
        expires_at_unix_ms: Some(expiry),
        bearer: RedactedString::new(format!("test-material-{generation}")), // awaken-allow(secret): deterministic fixture
    };
    let first = enter_or_rotate_application_mcp_bearer(command(1, 5_000), &store, &repo, &clock)
        .await
        .unwrap();
    assert!(matches!(
        enter_or_rotate_application_mcp_bearer(command(1, 6_000), &store, &repo, &clock,).await,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(repo.get(&first.id).await.unwrap(), first);
    let second = enter_or_rotate_application_mcp_bearer(command(2, 9_000), &store, &repo, &clock)
        .await
        .unwrap();
    assert_eq!(second.version, first.version + 1);
    assert_eq!(expiry(&second), Some(9_000));
    assert_ne!(second.material_ref, first.material_ref);
}

// E1 exact expiry replays without publication; E2 equal-generation expiry
// extension, shortening or removal rejects without writes; E3 a new generation
// atomically publishes material + expiry + revision; E4 expired input creates
// no new revision, material or rollout. The actual Managed entry/WAL is used.
#[tokio::test]
async fn managed_expiry_is_exact_generation_metadata() {
    use crate::catalog::ManagedVaultRepo;
    let repo = Arc::new(InMemoryCredentialRepo::new());
    let store = Arc::new(crate::InMemorySecretStore::new());
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_000));
    let app = crate::VaultApplication::new(store.clone(), repo.clone(), clock.clone(), None);
    for invalid in [0, 1_000] {
        assert!(
            app.enter_application_mcp_bearer(entry(1, Some(invalid)))
                .await
                .is_err()
        );
        assert!(repo.list("workspace").await.unwrap().is_empty());
        assert!(repo.list_vaults("workspace").await.unwrap().is_empty());
        assert!(store.inventory().await.unwrap().is_empty());
    }
    let expected_source = entry(1, Some(5_000))
        .target
        .source_id("workspace", "expiry-application");
    let first = app
        .enter_application_mcp_bearer(entry(1, Some(5_000)))
        .await
        .unwrap();
    let before = repo.get(&first.source_id).await.unwrap();
    assert_eq!(first.source_id, expected_source);
    let retained_pin = compiled(&before);
    let retained_wire = serde_json::to_value(&retained_pin).unwrap();
    assert_eq!(retained_pin.issuer_generation, Some(1));
    let material = store.inventory().await.unwrap();
    assert_eq!(expiry(&before), Some(5_000));
    assert_eq!(first.revision, 1);
    assert_eq!(
        app.enter_application_mcp_bearer(entry(1, Some(5_000)))
            .await
            .unwrap(),
        first,
        "E1 exact replay"
    );
    for changed in [Some(6_000), Some(4_000), None] {
        assert!(matches!(
            app.enter_application_mcp_bearer(entry(1, changed)).await,
            Err(CredentialError::MutationConflict(_))
        ));
        assert_eq!(repo.get(&first.source_id).await.unwrap(), before, "E2");
        assert_eq!(store.inventory().await.unwrap(), material, "E2");
        assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
        assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
    }
    let second = app
        .enter_application_mcp_bearer(entry(2, Some(9_000)))
        .await
        .unwrap();
    let after = repo.get(&second.source_id).await.unwrap();
    assert_eq!(second.source_id, first.source_id);
    assert_eq!(second.revision, first.revision + 1);
    assert_eq!(expiry(&after), Some(9_000), "E3");
    let current_pin = compiled(&after);
    assert_eq!(current_pin.issuer_generation, Some(2));
    assert_eq!(current_pin.expires_at_unix_ms, Some(9_000));
    assert_eq!(
        serde_json::from_value::<awaken_agent_contract::credential::CredentialAccess>(
            retained_wire
        )
        .unwrap(),
        retained_pin,
        "old Active pin retains its own issuer generation and expiry"
    );
    assert_eq!(retained_pin.expires_at_unix_ms, Some(5_000));
    assert_ne!(after.material_ref, before.material_ref);
    assert_eq!(
        store
            .get(after.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "test-bearer-2"
    );
    let events = repo.pending_managed_rollouts().await.unwrap();
    let material = store.inventory().await.unwrap();
    for invalid in [Some(0), Some(999), Some(1_000)] {
        assert!(matches!(
            app.enter_application_mcp_bearer(entry(3, invalid)).await,
            Err(CredentialError::InvalidSource(_))
        ));
        assert_eq!(repo.get(&second.source_id).await.unwrap(), after, "E4");
        assert_eq!(store.inventory().await.unwrap(), material, "E4");
        assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);
    }
    clock.set(9_000);
    assert!(
        app.enter_application_mcp_bearer(entry(2, Some(9_000)))
            .await
            .is_err(),
        "E4 exact replay cannot revive an expired generation"
    );
    assert_eq!(repo.get(&second.source_id).await.unwrap(), after);
}

#[derive(Default)]
struct FailNextPut {
    inner: crate::InMemorySecretStore,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl SecretStore for FailNextPut {
    async fn put(
        &self,
        reference: &crate::SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(CredentialError::Storage("injected material failure".into()));
        }
        self.inner.put(reference, secret).await
    }

    async fn get(&self, reference: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
        self.inner.get(reference).await
    }

    async fn delete(&self, reference: &crate::SecretRef) -> Result<(), CredentialError> {
        self.inner.delete(reference).await
    }

    async fn inventory(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        self.inner.inventory().await
    }
}

#[tokio::test]
async fn failed_material_rotation_cannot_extend_the_published_expiry() {
    let repo = Arc::new(InMemoryCredentialRepo::new());
    let store = Arc::new(FailNextPut::default());
    let app = crate::VaultApplication::new(
        store.clone(),
        repo.clone(),
        Arc::new(awaken_lease_testkit::ManualClock::new(1_000)),
        None,
    );
    let first = app
        .enter_application_mcp_bearer(entry(1, Some(5_000)))
        .await
        .unwrap();
    let before = repo.get(&first.source_id).await.unwrap();
    let material = store.inventory().await.unwrap();
    store.fail.store(true, Ordering::SeqCst);
    assert!(
        app.enter_application_mcp_bearer(entry(2, Some(9_000)))
            .await
            .is_err()
    );
    assert_eq!(repo.get(&first.source_id).await.unwrap(), before);
    assert_eq!(store.inventory().await.unwrap(), material);
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
    assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
    let retried = app
        .enter_application_mcp_bearer(entry(2, Some(9_000)))
        .await
        .unwrap();
    assert_eq!(retried.revision, 2);
    assert_eq!(
        expiry(&repo.get(&retried.source_id).await.unwrap()),
        Some(9_000)
    );
}

struct RecordingTarget {
    repo: Arc<InMemoryCredentialRepo>,
    pending: bool,
    calls: std::sync::Mutex<Vec<awaken_credential_contract::ManagedCredentialRollout>>,
}

impl RecordingTarget {
    fn new(repo: Arc<InMemoryCredentialRepo>, pending: bool) -> Self {
        Self {
            repo,
            pending,
            calls: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::ManagedCredentialRolloutTarget for RecordingTarget {
    async fn rollout(
        &self,
        event: &awaken_credential_contract::ManagedCredentialRollout,
    ) -> Result<
        awaken_credential_contract::ManagedCredentialAdoptionProgress,
        awaken_credential_contract::ManagedCredentialAdoptionError,
    > {
        assert!(
            self.repo
                .pending_managed_rollouts()
                .await
                .unwrap()
                .contains(event),
            "the durable event must be rearmed before participant I/O"
        );
        self.calls.lock().unwrap().push(event.clone());
        Ok(if self.pending {
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending
        } else {
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Converged
        })
    }
}

#[tokio::test]
async fn per_call_target_replays_exact_pending_rollout_without_replacing_background_target() {
    use awaken_credential_contract::ManagedCredentialAdoptionProgress as Progress;
    let repo = Arc::new(InMemoryCredentialRepo::new());
    let app = crate::VaultApplication::new(
        Arc::new(crate::InMemorySecretStore::new()),
        repo.clone(),
        Arc::new(awaken_lease_testkit::ManualClock::new(1_000)),
        None,
    );
    let background = Arc::new(RecordingTarget::new(repo.clone(), false));
    let explicit = RecordingTarget::new(repo.clone(), true);
    app.set_rollout_target(background.clone());
    app.enter_application_mcp_bearer(entry(1, Some(5_000)))
        .await
        .unwrap();
    let pending = app
        .enter_application_mcp_bearer_with_rollout_target(entry(2, Some(9_000)), &explicit)
        .await
        .unwrap();
    assert_eq!(pending.adoption, Progress::Pending);
    assert_eq!(explicit.calls.lock().unwrap().len(), 1);
    assert!(background.calls.lock().unwrap().is_empty());
    assert_eq!(
        app.enter_application_mcp_bearer_with_rollout_target(entry(2, Some(9_000)), &explicit)
            .await
            .unwrap(),
        pending
    );
    assert_eq!(explicit.calls.lock().unwrap().len(), 2);
    assert_eq!(repo.pending_managed_rollouts().await.unwrap().len(), 1);
    assert_eq!(app.reconcile_rollouts().await.unwrap(), 1);
    assert_eq!(background.calls.lock().unwrap().len(), 1);
    assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
    let replay = app
        .enter_application_mcp_bearer_with_rollout_target(entry(2, Some(9_000)), &explicit)
        .await
        .unwrap();
    assert_eq!(replay.revision, pending.revision);
    assert_eq!(replay.adoption, Progress::Pending);
    assert_eq!(explicit.calls.lock().unwrap().len(), 3);
    assert_eq!(background.calls.lock().unwrap().len(), 1);
    assert_eq!(repo.pending_managed_rollouts().await.unwrap().len(), 1);
}
